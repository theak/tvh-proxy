// tvh-proxy frontend. Plain JS, no build step.
(function () {
  'use strict';
  const L = window.TVLib;
  const $ = (id) => document.getElementById(id);
  const $$ = (sel, el) => Array.from((el || document).querySelectorAll(sel));

  // ------------------------------------------------------------ state
  const S = {
    client: null,
    channels: [],
    chIdx: new Map(),      // uuid -> index
    byCh: new Map(),       // uuid -> events
    keyword: null,
    view: null,            // last heartbeat view from the server
    playing: null,         // { id, generation, channel } currently attached
    current: null,         // channel uuid being shown
    pendingIdx: null,      // channel index while the user is flicking through
    commitTimer: 0,
    guideOpen: false,
    rows: [],              // guide cells per channel row
    win: null,
    focus: { row: 0, col: 0 },
    hls: null,
    lastProgressAt: 0,
    lastTime: 0,
    started: false,
    me: null,              // { username, admin }
  };

  try {
    S.client = sessionStorage.getItem('tvhp-client');
    if (!S.client) { S.client = L.newClientId(); sessionStorage.setItem('tvhp-client', S.client); }
  } catch (_) { S.client = L.newClientId(); }

  const now = () => Math.floor(Date.now() / 1000);

  // ------------------------------------------------------------ api
  async function api(path, body, opts) {
    opts = opts || {};
    const init = { credentials: 'same-origin', headers: {} };
    if (body !== undefined) {
      init.method = 'POST';
      init.headers['Content-Type'] = 'application/json';
      init.body = JSON.stringify(body);
      if (opts.keepalive) init.keepalive = true;
    }
    const r = await fetch(path, init);
    let data = null;
    try { data = await r.json(); } catch (_) { /* empty body */ }
    if (r.status === 401 && !opts.allow401) {
      stopEverything();
      showPage('login');
    }
    return { ok: r.ok, status: r.status, data: data || {} };
  }

  // ------------------------------------------------------------ pages
  function showPage(name) {
    $('boot').hidden = true;
    for (const p of ['setup', 'login', 'tv']) $(p).hidden = p !== name;
    if (name !== 'tv') document.title = 'TV';
    const first = name !== 'tv' && $(name).querySelector('input');
    if (first && window.matchMedia('(hover: hover)').matches) first.focus();
  }

  function formError(form, msg) {
    const el = form.querySelector('.form-error');
    el.textContent = msg || '';
    el.hidden = !msg;
  }

  function busy(form, on) {
    const btn = form.querySelector('[type=submit]');
    btn.disabled = on;
    btn.classList.toggle('busy', on);
  }

  function formData(form) {
    const o = {};
    for (const [k, v] of new FormData(form).entries()) o[k] = typeof v === 'string' ? v : '';
    return o;
  }

  $('setup-form').addEventListener('submit', async (e) => {
    e.preventDefault();
    const f = e.currentTarget;
    const d = formData(f);
    if (d.password !== d.confirm) return formError(f, "Passwords don't match.");
    if ((d.password || '').length < 8) return formError(f, 'Password must be at least 8 characters.');
    formError(f, '');
    busy(f, true);
    try {
      const r = await api('/api/setup', d, { allow401: true });
      if (!r.ok) return formError(f, r.data.error || 'Setup failed.');
      f.reset();
      startTV(true);
    } catch (_) {
      formError(f, "Couldn't reach the server.");
    } finally { busy(f, false); }
  });

  $('login-form').addEventListener('submit', async (e) => {
    e.preventDefault();
    const f = e.currentTarget;
    formError(f, '');
    busy(f, true);
    try {
      const r = await api('/api/login', formData(f), { allow401: true });
      if (!r.ok) return formError(f, r.data.error || 'Sign-in failed.');
      f.reset();
      startTV(true);
    } catch (_) {
      formError(f, "Couldn't reach the server.");
    } finally { busy(f, false); }
  });

  // ------------------------------------------------------------ guide data
  async function loadGuide() {
    const r = await api('/api/guide');
    if (!r.ok) {
      if (r.status !== 401) toast(r.data.error || "Couldn't load the guide.");
      return false;
    }
    S.channels = r.data.channels || [];
    S.chIdx = new Map(S.channels.map((c, i) => [c.uuid, i]));
    S.byCh = L.indexEvents(r.data.events || []);
    S.keyword = r.data.keyword || null;
    if (S.guideOpen) renderGuide(true);
    renderNow();
    return true;
  }

  const chan = (uuid) => S.channels[S.chIdx.get(uuid)];
  const chanLabel = (c) => (c ? (c.number ? c.number + ' · ' : '') + c.name : '');

  // ------------------------------------------------------------ viewer: now/next overlay
  function setText(id, text) { const el = $(id); if (el.textContent !== text) el.textContent = text; }

  function renderNow() {
    const t = now();
    const idx = S.pendingIdx != null ? S.pendingIdx : S.chIdx.get(S.current);
    const c = S.channels[idx];
    if (!c) return;
    const { cur, next } = L.nowNext(S.byCh, c.uuid, t);
    setText('cur-num', c.number || '');
    setText('cur-name', c.name);
    document.title = c.name + ' · TV';
    setText('cur-title', cur ? cur.title : c.name);
    const cat = $('cur-cat');
    cat.hidden = !(cur && cur.category);
    if (cur && cur.category) cat.textContent = cur.category;
    setText('cur-range', cur ? L.fmtClock(cur.start) + ' – ' + L.fmtClock(cur.stop) + ' · ' + L.fmtDur(cur.stop - cur.start) : 'No guide info');
    setText('cur-start', cur ? L.fmtClock(cur.start) : '');
    setText('cur-end', cur ? L.fmtClock(cur.stop) : '');
    setText('cur-left', cur ? L.fmtLeft(cur, t) : '');
    $('cur-bar').style.width = (L.progress(cur, t) * 100).toFixed(1) + '%';
    $('cur-bar').parentElement.style.visibility = cur ? 'visible' : 'hidden';
    const desc = cur ? [cur.subtitle, cur.description].filter(Boolean).join(' — ') : '';
    setText('cur-desc', desc);
    $('cur-desc').hidden = !desc;
    $('next-row').hidden = !next;
    if (next) { setText('next-time', L.fmtClock(next.start)); setText('next-title', next.title); }
    const up = S.channels[L.step(S.channels.length, idx, 1)];
    const dn = S.channels[L.step(S.channels.length, idx, -1)];
    for (const [pre, ch] of [['up', up], ['dn', dn]]) {
      setText(pre + '-num', ch ? ch.number || '' : '');
      setText(pre + '-name', ch ? ch.name : '');
      const nn = ch && L.nowNext(S.byCh, ch.uuid, t).cur;
      setText(pre + '-title', nn ? nn.title : '');
    }
    updateSwitcher();
  }

  // Up/down only show when there's a channel that way and this viewer holds the remote.
  function updateSwitcher() {
    const locked = !!(S.view && S.view.role === 'follower');
    const idx = S.pendingIdx != null ? S.pendingIdx : S.chIdx.get(S.current);
    for (const [id, dir] of [['ch-up', 1], ['ch-down', -1]]) {
      const show = !locked && idx != null && L.step(S.channels.length, idx, dir) >= 0;
      $(id).parentElement.style.visibility = show ? '' : 'hidden';
    }
  }

  // ------------------------------------------------------------ viewer: chrome, loader, toasts
  let chromeTimer = 0;
  function showChrome(sticky) {
    $('tv').classList.remove('chrome-hidden');
    clearTimeout(chromeTimer);
    if (!sticky) chromeTimer = setTimeout(hideChrome, 4500);
  }
  function hideChrome() {
    if (!$('tv').classList.contains('playing') || S.guideOpen || !$('settings').hidden) return;
    $('tv').classList.add('chrome-hidden');
  }

  function showLoader(uuid, stage) {
    const c = chan(uuid);
    $('loader').hidden = false;
    $('error-panel').hidden = true;
    const num = $('loader-num');
    const label = c ? c.number || '•' : '•';
    if (num.textContent !== label) {
      num.textContent = label;
      num.style.animation = 'none'; void num.offsetWidth; num.style.animation = '';
    }
    setText('loader-name', c ? c.name : '');
    setLoaderStage(stage || 'Tuning', 0.15);
  }
  function setLoaderStage(text, frac) {
    setText('loader-stage', text);
    $('loader-fill').style.transform = 'scaleX(' + frac + ')';
  }
  function hideLoader() { $('loader').hidden = true; }

  function showError(msg) {
    hideLoader();
    $('tv').classList.remove('playing');
    setText('error-msg', msg);
    $('error-panel').hidden = false;
    showChrome(true);
  }

  let toastTimer = 0;
  function toast(msg) {
    const t = $('toast');
    t.textContent = msg;
    t.hidden = false;
    t.style.animation = 'none'; void t.offsetWidth; t.style.animation = '';
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => { t.hidden = true; }, 3200);
  }

  let peekTimer = 0;
  function showPeek(dir, c) {
    const nn = L.nowNext(S.byCh, c.uuid, now()).cur;
    setText('peek-arrow', dir > 0 ? '↑' : '↓');
    setText('peek-hint', dir > 0 ? 'Next channel' : 'Previous channel');
    setText('peek-name', chanLabel(c));
    setText('peek-title', nn ? nn.title : '');
    $('peek').classList.add('show');
    clearTimeout(peekTimer);
    peekTimer = setTimeout(() => $('peek').classList.remove('show'), 1100);
  }

  // ------------------------------------------------------------ player
  const video = $('video');
  const nativeHls = () => !!video.canPlayType('application/vnd.apple.mpegurl');

  function detachPlayer() {
    if (S.hls) { S.hls.destroy(); S.hls = null; }
    video.removeAttribute('src');
    try { video.load(); } catch (_) { /* ignore */ }
    $('tv').classList.remove('playing');
  }

  function attach(sv) {
    detachPlayer();
    S.playing = { id: sv.id, generation: sv.generation, channel: sv.channel };
    S.lastProgressAt = Date.now();
    const url = sv.playlist;
    if (nativeHls()) {
      video.src = url;
    } else if (window.Hls && window.Hls.isSupported()) {
      const hls = new window.Hls({
        liveSyncDurationCount: 2,
        liveMaxLatencyDurationCount: 6,
        maxBufferLength: 8,
        backBufferLength: 10,
        manifestLoadPolicy: { default: { maxTimeToFirstByteMs: 25000, maxLoadTimeMs: 30000, timeoutRetry: { maxNumRetry: 3, retryDelayMs: 500, maxRetryDelayMs: 2000 }, errorRetry: { maxNumRetry: 6, retryDelayMs: 1000, maxRetryDelayMs: 4000 } } },
      });
      hls.on(window.Hls.Events.ERROR, (_e, data) => {
        if (!data.fatal) return;
        if (data.type === window.Hls.ErrorTypes.MEDIA_ERROR) hls.recoverMediaError();
        else setTimeout(() => { if (S.hls === hls) reattach(); }, 1500);
      });
      hls.loadSource(url);
      hls.attachMedia(video);
      S.hls = hls;
    } else {
      showError("This browser can't play live video.");
      return;
    }
    tryPlay();
  }

  function reattach() {
    const sv = S.view && S.view.stream;
    if (sv && sv.status.state !== 'error') attach(sv);
  }

  function tryPlay() {
    const p = video.play();
    if (p && p.catch) {
      p.catch((err) => {
        if (err && err.name === 'NotAllowedError' && !video.muted) {
          video.muted = true;
          $('sound').hidden = false;
          video.play().catch(() => {});
        }
      });
    }
  }

  video.addEventListener('playing', () => {
    S.autoRetry = null;
    $('tv').classList.add('playing');
    hideLoader();
    $('error-panel').hidden = true;
    showChrome();
  });
  video.addEventListener('timeupdate', () => {
    if (video.currentTime !== S.lastTime) { S.lastTime = video.currentTime; S.lastProgressAt = Date.now(); }
  });
  video.addEventListener('error', () => {
    if (S.hls) return; // hls.js reports its own errors
    setTimeout(() => { if (S.playing) reattach(); }, 1500);
  });
  $('sound').addEventListener('click', (e) => {
    e.stopPropagation();
    video.muted = false;
    $('sound').hidden = true;
    tryPlay();
  });

  // Stall watchdog: live video that stops advancing for a while gets re-attached
  // (jumps back to the live edge). Covers iOS resuming from the background too.
  setInterval(() => {
    if (!S.playing || document.hidden || video.paused && !$('tv').classList.contains('playing')) return;
    if (Date.now() - S.lastProgressAt > 12000 && S.view && S.view.stream && S.view.stream.status.state === 'live') {
      S.lastProgressAt = Date.now();
      reattach();
    }
  }, 3000);

  // ------------------------------------------------------------ server view / roles
  function applyView(v) {
    S.view = v;
    const locked = v.role === 'follower';
    $('tv').classList.toggle('locked', locked);
    $('lock-banner').hidden = !locked;
    updateSwitcher();
    updateWatchButton();
    const sv = v.stream;
    if (!sv) {
      if (S.playing) {
        S.playing = null;
        detachPlayer();
        openGuide();
      }
      return;
    }
    if (S.pendingIdx == null) S.current = sv.channel;
    if (sv.status.state === 'error') {
      if (S.playing && S.playing.generation === sv.generation) detachPlayer();
      S.playing = { id: sv.id, generation: sv.generation, channel: sv.channel };
      // The USB tuner often stalls briefly after a retune; one quiet retry usually
      // fixes it. Only the controller retries, once per failed tuning.
      const auto = S.autoRetry;
      if (v.role === 'controller' && !(auto && auto.channel === sv.channel && auto.used)) {
        if (!auto || auto.generation !== sv.generation) {
          S.autoRetry = { channel: sv.channel, generation: sv.generation, used: false };
          showLoader(sv.channel, 'Tuner hiccup — retrying');
          setLoaderStage('Tuner hiccup — retrying', 0.5);
          setTimeout(() => {
            if (!S.autoRetry || S.autoRetry.generation !== sv.generation || S.current !== sv.channel) return;
            S.autoRetry.used = true;
            watchChannel(sv.channel, { retry: true });
          }, 6000);
        }
      } else {
        showError(sv.status.message);
      }
    } else if (!S.playing || S.playing.id !== sv.id || S.playing.generation !== sv.generation) {
      showLoader(sv.channel, sv.status.state === 'live' ? 'Buffering' : 'Tuning');
      attach(sv);
    } else if (!$('tv').classList.contains('playing') && $('error-panel').hidden) {
      setLoaderStage(sv.status.state === 'live' ? 'Buffering' : 'Tuning', sv.status.state === 'live' ? 0.7 : 0.35);
    }
    renderNow();
  }

  async function heartbeat() {
    if (!S.started) return;
    try {
      const r = await api('/api/heartbeat', { client: S.client });
      if (r.ok) applyView(r.data);
    } catch (_) { /* offline for a moment; try again next tick */ }
  }

  async function watchChannel(uuid, opts) {
    opts = opts || {};
    if (S.view && S.view.role === 'follower') { flashLock(); return; }
    S.current = uuid;
    S.pendingIdx = null;
    renderNow();
    showLoader(uuid, 'Tuning');
    if (S.playing && S.playing.channel !== uuid) detachPlayer();
    try {
      const r = await api('/api/watch', { client: S.client, channel: uuid, retry: !!opts.retry });
      if (r.ok) applyView(r.data);
      else if (r.status === 409) { applyView(r.data); flashLock(); }
      else if (r.status !== 401) showError(r.data.error || "Couldn't start that channel.");
    } catch (_) {
      showError("Couldn't reach the server.");
    }
  }

  function flashLock() {
    const b = $('lock-banner');
    b.hidden = false;
    b.animate([{ transform: 'translateX(-50%)' }, { transform: 'translateX(calc(-50% - 6px))' }, { transform: 'translateX(calc(-50% + 6px))' }, { transform: 'translateX(-50%)' }], { duration: 320 });
  }

  // Flicking through channels: preview immediately, tune once the user settles.
  // The debounce also protects a fragile single tuner from rapid retuning.
  function changeChannel(dir) {
    if (!S.channels.length) return;
    if (S.view && S.view.role === 'follower') { flashLock(); return; }
    const base = S.pendingIdx != null ? S.pendingIdx : (S.chIdx.get(S.current) ?? 0);
    const next = L.step(S.channels.length, base, dir);
    if (next < 0) return;
    S.pendingIdx = next;
    const c = S.channels[S.pendingIdx];
    showPeek(dir, c);
    showChrome();
    renderNow();
    clearTimeout(S.commitTimer);
    S.commitTimer = setTimeout(() => {
      const target = S.channels[S.pendingIdx];
      S.pendingIdx = null;
      if (target && target.uuid !== (S.view && S.view.stream && S.view.stream.channel)) watchChannel(target.uuid);
      else renderNow();
    }, 700);
  }

  // Stop: end playback, free the tuner (the server stops it at once if nobody else
  // is watching) and land on the guide. Browsers don't let a page close its own
  // tab or an iOS home-screen app, so the guide is where "exit" goes.
  async function stopWatching() {
    clearTimeout(S.commitTimer);
    S.pendingIdx = null;
    S.autoRetry = null;
    S.playing = null;
    detachPlayer();
    hideLoader();
    $('error-panel').hidden = true;
    openGuide();
    try {
      const r = await api('/api/stop', { client: S.client });
      if (r.ok) applyView(r.data);
    } catch (_) { /* the next heartbeat will catch up */ }
  }
  $('stop').addEventListener('click', (e) => { e.stopPropagation(); stopWatching(); });

  $('ch-up').addEventListener('click', (e) => { e.stopPropagation(); changeChannel(1); });
  $('ch-down').addEventListener('click', (e) => { e.stopPropagation(); changeChannel(-1); });
  $('retry').addEventListener('click', () => {
    const ch = (S.view && S.view.stream && S.view.stream.channel) || S.current;
    if (ch) watchChannel(ch, { retry: true });
  });

  // ------------------------------------------------------------ touch & pointer on the stage
  const stage = document.querySelector('.stage');
  let touch = null;
  stage.addEventListener('touchstart', (e) => {
    if (e.target.closest('button, a, input') || e.touches.length !== 1) { touch = null; return; }
    touch = { y: e.touches[0].clientY, x: e.touches[0].clientX };
  }, { passive: true });
  // Tap toggles the overlay. Channel changes are buttons/keys only (no swipe), so a
  // stray drag can't retune the tuner.
  stage.addEventListener('touchend', (e) => {
    if (!touch) return;
    const t = e.changedTouches[0];
    const moved = Math.abs(t.clientY - touch.y) >= 10 || Math.abs(t.clientX - touch.x) >= 10;
    touch = null;
    if (moved) return;
    if ($('tv').classList.contains('chrome-hidden')) showChrome(); else hideChromeNow();
  });
  function hideChromeNow() { clearTimeout(chromeTimer); hideChrome(); }
  stage.addEventListener('mousemove', () => showChrome());
  stage.addEventListener('click', (e) => {
    if (e.target.closest('button')) return;
    if (window.matchMedia('(hover: hover)').matches) showChrome();
  });
  stage.addEventListener('dblclick', (e) => {
    if (e.target.closest('button')) return;
    toggleFullscreen();
  });

  function toggleFullscreen() {
    const el = document.documentElement;
    if (document.fullscreenElement) document.exitFullscreen().catch(() => {});
    else if (el.requestFullscreen) el.requestFullscreen().catch(() => {});
    else if (video.webkitEnterFullscreen) video.webkitEnterFullscreen();
  }

  // ------------------------------------------------------------ guide
  function metrics() {
    const narrow = window.innerWidth < 700;
    return { pxPerMin: narrow ? 4.4 : 7, chW: narrow ? 128 : 248, rowH: narrow ? 62 : 72, hours: 8 };
  }

  function openGuide() {
    // Check what's on screen, not just the flag, so a stale flag can't block it.
    if (S.guideOpen && $('tv').classList.contains('guide-open')) return;
    S.guideOpen = true;
    $('tv').classList.add('guide-open');
    $('guide').setAttribute('aria-hidden', 'false');
    showChrome(true);
    renderGuide(false);
  }
  function hideGuide() {
    S.guideOpen = false;
    $('tv').classList.remove('guide-open');
    $('guide').setAttribute('aria-hidden', 'true');
    showChrome();
  }
  function closeGuide() {
    if (!S.guideOpen) return;
    if (!S.playing && !(S.view && S.view.stream)) { toast('Pick something to watch first.'); return; }
    hideGuide();
  }
  $$('[data-open-guide]').forEach((b) => b.addEventListener('click', (e) => { e.stopPropagation(); openGuide(); }));
  $('close-guide').addEventListener('click', closeGuide);

  function renderGuide(keepFocus) {
    const m = metrics();
    const t = now();
    S.win = L.guideWindow(t, m.hours);
    const tlW = Math.round(m.hours * 60 * m.pxPerMin);
    const inner = $('grid-inner');
    inner.style.width = (m.chW + tlW) + 'px';
    const frag = document.createDocumentFragment();

    const ruler = document.createElement('div');
    ruler.className = 'ruler';
    ruler.innerHTML = '<div class="ruler-label" style="width:' + m.chW + 'px">Channels</div><div class="ruler-track" style="width:' + tlW + 'px"></div>';
    const track = ruler.lastChild;
    for (const tk of L.ticks(S.win, m.pxPerMin)) {
      const d = document.createElement('div');
      d.className = 'tick';
      d.style.left = tk.left + 'px';
      d.textContent = tk.label;
      track.appendChild(d);
    }
    const nowX = Math.round(((t - S.win.start) / 60) * m.pxPerMin);
    const chip = document.createElement('div');
    chip.className = 'now-chip';
    chip.style.left = Math.max(30, nowX) + 'px'; // keep the label clear of the sticky channel column
    chip.textContent = L.fmtClock(t);
    track.appendChild(chip);
    frag.appendChild(ruler);

    const line = document.createElement('div');
    line.className = 'now-line';
    line.style.left = (m.chW + nowX) + 'px';
    frag.appendChild(line);

    S.rows = [];
    const playingCh = S.view && S.view.stream ? S.view.stream.channel : S.current;
    S.channels.forEach((c, r) => {
      let cells = L.layoutRow(S.byCh.get(c.uuid), S.win, m.pxPerMin);
      if (!cells.length) cells = [{ ev: { channel: c.uuid, start: S.win.start, stop: S.win.end, title: c.name, synthetic: true }, left: 2, width: tlW - 4 }];
      S.rows.push(cells);
      const row = document.createElement('div');
      row.className = 'grow' + (c.uuid === playingCh ? ' current' : '');
      row.style.height = m.rowH + 'px';
      const ch = document.createElement('button');
      ch.type = 'button';
      ch.className = 'gch';
      ch.style.width = m.chW + 'px';
      ch.dataset.row = r;
      ch.innerHTML = '<span class="gch-logo blank"></span><span class="gch-text"><span class="gch-num"></span><span class="gch-name"></span></span>';
      if (c.icon) {
        // Show the light logo plate only once the image actually loads; missing
        // logos (tvheadend 404s) keep the neutral tile.
        const img = new Image();
        img.className = 'gch-logo';
        img.alt = '';
        img.decoding = 'async';
        img.addEventListener('load', () => ch.querySelector('.gch-logo.blank')?.replaceWith(img), { once: true });
        img.src = c.icon;
      }
      ch.querySelector('.gch-num').textContent = c.number || '';
      ch.querySelector('.gch-name').textContent = c.name;
      row.appendChild(ch);
      const tr = document.createElement('div');
      tr.className = 'gtrack';
      tr.style.width = tlW + 'px';
      cells.forEach((cell, col) => {
        const b = document.createElement('button');
        b.type = 'button';
        const e = cell.ev;
        b.className = 'gcell' + (e.start <= t && t < e.stop ? ' airing' : '') + (e.stop <= t ? ' past' : '') + (cell.width < 70 ? ' narrow' : '');
        b.style.left = cell.left + 'px';
        b.style.width = cell.width + 'px';
        b.dataset.row = r;
        b.dataset.col = col;
        b.innerHTML = '<span class="gcell-title"></span><span class="gcell-time"></span>';
        b.querySelector('.gcell-title').textContent = e.title;
        b.querySelector('.gcell-time').textContent = e.synthetic ? 'No guide info' : L.fmtClock(e.start) + ' – ' + L.fmtClock(e.stop);
        tr.appendChild(b);
      });
      row.appendChild(tr);
      frag.appendChild(row);
    });
    inner.replaceChildren(frag);
    setText('g-date', L.fmtDate(t));

    if (!keepFocus || S.focus.row >= S.rows.length) {
      const row = Math.max(0, S.chIdx.has(playingCh) ? S.chIdx.get(playingCh) : 0);
      S.focus = { row, col: L.focusIndexFor(S.rows[row] || [], t) };
    }
    S.focus.col = Math.min(S.focus.col, (S.rows[S.focus.row] || [1]).length - 1);
    applyFocus(!keepFocus);
  }

  function focusedEl() {
    return $('grid-inner').querySelector('.gcell[data-row="' + S.focus.row + '"][data-col="' + S.focus.col + '"]');
  }

  function applyFocus(scroll) {
    $$('.gcell.focused', $('grid-inner')).forEach((el) => el.classList.remove('focused'));
    const el = focusedEl();
    if (el) el.classList.add('focused');
    renderHero();
    if (scroll && el) scrollIntoGrid(el, true);
    else if (el) scrollIntoGrid(el, false);
  }

  function scrollIntoGrid(el, center) {
    const grid = $('grid');
    const m = metrics();
    const row = el.parentElement.parentElement;
    const top = row.offsetTop;
    const rh = row.offsetHeight;
    if (center) grid.scrollTop = Math.max(0, top - grid.clientHeight / 2 + rh / 2);
    else if (top < grid.scrollTop + 40) grid.scrollTop = top - 40;
    else if (top + rh > grid.scrollTop + grid.clientHeight) grid.scrollTop = top + rh - grid.clientHeight;
    const left = m.chW + el.offsetLeft;
    const right = left + el.offsetWidth;
    const viewL = grid.scrollLeft + m.chW;
    const viewR = grid.scrollLeft + grid.clientWidth;
    // The window starts at the half hour before now, so scrolling to its start keeps
    // the airing programme's title visible.
    if (center) grid.scrollLeft = 0;
    else if (left < viewL) grid.scrollLeft = Math.max(0, left - m.chW - 24);
    else if (right > viewR && el.offsetWidth < grid.clientWidth - m.chW) grid.scrollLeft = right - grid.clientWidth + 24;
  }

  function renderHero() {
    const cells = S.rows[S.focus.row];
    const c = S.channels[S.focus.row];
    if (!cells || !c) return;
    const e = cells[S.focus.col].ev;
    const t = now();
    setText('h-num', c.number || '');
    setText('h-name', c.name);
    const st = $('h-status');
    const live = !e.synthetic && e.start <= t && t < e.stop;
    st.className = 'status-pill' + (live ? '' : ' later');
    st.textContent = e.synthetic ? 'Live' : live ? 'On now' : e.stop <= t ? 'Ended' : 'Starts ' + L.fmtClock(e.start);
    setText('h-title', e.title);
    setText('h-meta', e.synthetic ? 'No guide information for this channel' : L.fmtClock(e.start) + ' – ' + L.fmtClock(e.stop) + ' · ' + L.fmtDur(e.stop - e.start) + (e.category ? ' · ' + e.category : ''));
    $('h-bar-wrap').hidden = !live;
    $('h-bar').style.width = (L.progress(e, t) * 100).toFixed(1) + '%';
    const desc = [e.subtitle, e.description].filter(Boolean).join(' — ');
    setText('h-desc', desc);
    $('h-desc').hidden = !desc;
    updateWatchButton();
  }

  function updateWatchButton() {
    const c = S.channels[S.focus.row];
    const btn = $('watch-focused');
    const locked = S.view && S.view.role === 'follower';
    const watching = c && S.view && S.view.stream && S.view.stream.channel === c.uuid;
    btn.disabled = !!locked && !watching;
    setText('watch-label', locked && !watching ? 'Locked' : watching ? 'Back to it' : 'Watch ' + (c ? c.name : ''));
  }

  function watchFocused() {
    const c = S.channels[S.focus.row];
    if (!c) return;
    const watching = S.view && S.view.stream && S.view.stream.channel === c.uuid;
    if (watching) { closeGuide(); return; }
    if (S.view && S.view.role === 'follower') { flashLock(); return; }
    hideGuide();
    watchChannel(c.uuid);
  }
  $('watch-focused').addEventListener('click', watchFocused);

  $('grid-inner').addEventListener('click', (e) => {
    const cell = e.target.closest('.gcell');
    const ch = e.target.closest('.gch');
    if (cell) {
      const row = +cell.dataset.row;
      const col = +cell.dataset.col;
      if (S.focus.row === row && S.focus.col === col) { watchFocused(); return; }
      S.focus = { row, col };
      applyFocus(false);
    } else if (ch) {
      const row = +ch.dataset.row;
      S.focus = { row, col: L.focusIndexFor(S.rows[row], now()) };
      applyFocus(false);
      watchFocused();
    }
  });

  function moveFocus(dr, dc) {
    if (!S.rows.length) return;
    const cur = S.rows[S.focus.row][S.focus.col];
    if (dr) {
      const row = Math.min(S.rows.length - 1, Math.max(0, S.focus.row + dr));
      const anchor = Math.max(cur.ev.start, now());
      S.focus = { row, col: L.cellAtTime(S.rows[row], anchor) };
    } else {
      const cells = S.rows[S.focus.row];
      S.focus.col = Math.min(cells.length - 1, Math.max(0, S.focus.col + dc));
    }
    applyFocus(false);
  }

  // ------------------------------------------------------------ keyboard
  document.addEventListener('keydown', (e) => {
    if ($('tv').hidden || e.metaKey || e.ctrlKey || e.altKey) return;
    if (!$('settings').hidden) { if (e.key === 'Escape') closeSettings(); return; }
    if (e.target.closest && e.target.closest('input, textarea')) return;
    const k = e.key;
    if (S.guideOpen) {
      if (k === 'Escape' || k === 'g' || k === 'G') { e.preventDefault(); closeGuide(); }
      else if (k === 'ArrowUp') { e.preventDefault(); moveFocus(-1, 0); }
      else if (k === 'ArrowDown') { e.preventDefault(); moveFocus(1, 0); }
      else if (k === 'ArrowLeft') { e.preventDefault(); moveFocus(0, -1); }
      else if (k === 'ArrowRight') { e.preventDefault(); moveFocus(0, 1); }
      else if (k === 'Enter') { e.preventDefault(); watchFocused(); }
      return;
    }
    if (k === 'ArrowUp') { e.preventDefault(); changeChannel(1); }
    else if (k === 'ArrowDown') { e.preventDefault(); changeChannel(-1); }
    else if (k === 'g' || k === 'G') { e.preventDefault(); openGuide(); }
    else if (k === 's' || k === 'S') { e.preventDefault(); stopWatching(); }
    else if (k === 'm' || k === 'M') { video.muted = !video.muted; $('sound').hidden = true; toast(video.muted ? 'Muted' : 'Sound on'); }
    else if (k === 'f' || k === 'F') toggleFullscreen();
    else showChrome();
  });

  // ------------------------------------------------------------ settings (admin only)
  // A full page. Opening it stops playback first (same as ✕), so nothing is left
  // running on the tuner behind it; closing it returns to the guide.
  async function openSettings() {
    if (!S.me || !S.me.admin) return;
    await stopWatching();
    const r = await api('/api/settings');
    if (!r.ok) { toast(r.data.error || "Couldn't load settings."); return; }
    const f = $('settings-form');
    f.reset();
    formError(f, '');
    f.tvh_url.value = r.data.tvh_url;
    f.tvh_username.value = r.data.tvh_username || '';
    f.tvh_password.placeholder = r.data.has_tvh_password ? 'unchanged' : '(none)';
    f.keywords.value = r.data.keywords;
    $('add-account').reset();
    formError($('add-account'), '');
    renderAccounts(r.data.accounts || []);
    setText('signed-in-as', 'Signed in as ' + r.data.username);
    $('settings').hidden = false;
    $('settings').scrollTop = 0;
    showChrome(true);
  }
  function closeSettings() { $('settings').hidden = true; showChrome(); }
  $$('[data-open-settings]').forEach((b) => b.addEventListener('click', (e) => { e.stopPropagation(); openSettings(); }));
  $('close-settings').addEventListener('click', closeSettings);

  $('settings-form').addEventListener('submit', async (e) => {
    e.preventDefault();
    const f = e.currentTarget;
    const d = formData(f);
    const body = {
      tvh_url: d.tvh_url, tvh_username: d.tvh_username, keywords: d.keywords,
      current_password: d.current_password, new_password: d.new_password, confirm: d.confirm,
    };
    if (d.tvh_password) body.tvh_password = d.tvh_password;
    if (d.new_password && d.new_password !== d.confirm) return formError(f, "New passwords don't match.");
    formError(f, '');
    busy(f, true);
    try {
      const r = await api('/api/settings', body);
      if (!r.ok) return formError(f, r.data.error || "Couldn't save.");
      f.current_password.value = f.new_password.value = f.confirm.value = f.tvh_password.value = '';
      toast('Saved');
      loadGuide();
    } finally { busy(f, false); }
  });

  function renderAccounts(list) {
    const box = $('accounts');
    box.replaceChildren();
    for (const a of list) {
      const row = document.createElement('div');
      row.className = 'acct';
      row.dataset.id = a.id;
      row.innerHTML =
        '<div class="acct-row"><div class="acct-name"><span class="n"></span></div><div class="acct-actions">' +
        '<button type="button" class="btn-glass small" data-act="priority"></button>' +
        '<button type="button" class="btn-glass small" data-act="reset">Reset password</button>' +
        (a.admin ? '' : '<button type="button" class="btn-glass small danger" data-act="delete">Delete</button>') +
        '</div></div>' +
        '<form class="acct-reset" novalidate hidden><div class="row2">' +
        '<label class="field"><span class="label">New password</span><input name="password" type="password" autocomplete="new-password"></label>' +
        '<label class="field"><span class="label">Confirm</span><input name="confirm" type="password" autocomplete="new-password"></label>' +
        '</div><p class="form-error" role="alert" hidden></p><div class="sp-actions"><button type="submit" class="btn-primary small"><span>Set password</span></button></div></form>';
      row.querySelector('.n').textContent = a.username;
      const name = row.querySelector('.acct-name');
      for (const [on, label, cls] of [[a.admin, 'Admin', ''], [a.priority, 'Priority', ' priority'], [a.me, 'You', '']]) {
        if (!on) continue;
        const tag = document.createElement('span');
        tag.className = 'tag' + cls;
        tag.textContent = label;
        name.appendChild(tag);
      }
      row.querySelector('[data-act="priority"]').textContent = a.priority ? 'Remove priority' : 'Make priority';
      row.querySelector('[data-act="priority"]').dataset.value = a.priority ? 'false' : 'true';
      box.appendChild(row);
    }
  }

  async function accountCall(method, path, body) {
    const init = { method, credentials: 'same-origin', headers: {} };
    if (body) { init.headers['Content-Type'] = 'application/json'; init.body = JSON.stringify(body); }
    const r = await fetch(path, init);
    let data = {};
    try { data = await r.json(); } catch (_) { /* empty */ }
    if (r.status === 401) { stopEverything(); showPage('login'); }
    return { ok: r.ok, data };
  }

  $('accounts').addEventListener('click', async (e) => {
    const btn = e.target.closest('button[data-act]');
    if (!btn) return;
    const row = btn.closest('.acct');
    const id = row.dataset.id;
    const act = btn.dataset.act;
    if (act === 'reset') {
      const f = row.querySelector('.acct-reset');
      f.hidden = !f.hidden;
      if (!f.hidden) f.password.focus();
      return;
    }
    if (act === 'delete' && !btn.classList.contains('armed')) {
      // Second tap confirms (no browser dialogs).
      btn.classList.add('armed');
      btn.textContent = 'Tap again to delete';
      setTimeout(() => { if (btn.isConnected) { btn.classList.remove('armed'); btn.textContent = 'Delete'; } }, 4000);
      return;
    }
    btn.disabled = true;
    const r = act === 'delete'
      ? await accountCall('DELETE', '/api/accounts/' + encodeURIComponent(id))
      : await accountCall('POST', '/api/accounts/' + encodeURIComponent(id), { priority: btn.dataset.value === 'true' });
    btn.disabled = false;
    if (!r.ok) { toast(r.data.error || "Couldn't update that account."); return; }
    renderAccounts(r.data.accounts);
    toast(act === 'delete' ? 'Account deleted' : 'Saved');
  });

  $('accounts').addEventListener('submit', async (e) => {
    const f = e.target.closest('.acct-reset');
    if (!f) return;
    e.preventDefault();
    const d = formData(f);
    if (d.password !== d.confirm) return formError(f, "Passwords don't match.");
    formError(f, '');
    busy(f, true);
    try {
      const id = f.closest('.acct').dataset.id;
      const r = await accountCall('POST', '/api/accounts/' + encodeURIComponent(id), { password: d.password, confirm: d.confirm });
      if (!r.ok) return formError(f, r.data.error || "Couldn't set the password.");
      renderAccounts(r.data.accounts);
      toast('Password set — that account was signed out everywhere');
    } finally { busy(f, false); }
  });

  $('add-account').addEventListener('submit', async (e) => {
    e.preventDefault();
    const f = e.currentTarget;
    const d = formData(f);
    if (d.password !== d.confirm) return formError(f, "Passwords don't match.");
    formError(f, '');
    busy(f, true);
    try {
      const r = await accountCall('POST', '/api/accounts', { username: d.username, password: d.password, confirm: d.confirm, priority: f.priority.checked });
      if (!r.ok) return formError(f, r.data.error || "Couldn't add the account.");
      f.reset();
      renderAccounts(r.data.accounts);
      toast('Account added');
    } finally { busy(f, false); }
  });

  async function signOut() {
    await api('/api/leave', { client: S.client }).catch(() => {});
    await api('/api/logout', {}, { allow401: true }).catch(() => {});
    location.reload();
  }
  $('logout').addEventListener('click', signOut);
  $('guide-logout').addEventListener('click', signOut);

  // ------------------------------------------------------------ lifecycle
  let timers = [];
  function stopEverything() {
    S.started = false;
    timers.forEach(clearInterval);
    timers = [];
    detachPlayer();
    S.playing = null;
  }

  function tickClock() {
    const t = L.fmtClock(now());
    $$('[data-clock]').forEach((el) => { if (el.textContent !== t) el.textContent = t; });
  }

  async function startTV(gesture) {
    showPage('tv');
    S.started = true;
    try {
      const st = await api('/api/status', undefined, { allow401: true });
      S.me = st.data.authed ? { username: st.data.username, admin: !!st.data.admin } : null;
    } catch (_) { S.me = null; }
    document.body.classList.toggle('is-admin', !!(S.me && S.me.admin));
    tickClock();
    if (gesture) video.muted = false;
    const ok = await loadGuide();
    if (!S.started) return;
    timers.push(setInterval(tickClock, 1000));
    timers.push(setInterval(heartbeat, 4000));
    timers.push(setInterval(() => { renderNow(); if (S.guideOpen) renderHero(); }, 15000));
    timers.push(setInterval(loadGuide, 60000));
    const r = await api('/api/heartbeat', { client: S.client });
    if (!r.ok) return;
    if (r.data.stream) { applyView(r.data); return; }
    applyView(r.data);
    if (ok && S.keyword && chan(S.keyword.channel)) {
      const c = chan(S.keyword.channel);
      watchChannel(c.uuid);
      setTimeout(() => toast('Matched “' + S.keyword.keyword + '” — tuned to ' + c.name), 400);
    } else {
      openGuide();
    }
  }

  window.addEventListener('pagehide', () => {
    if (!S.started) return;
    try {
      fetch('/api/leave', { method: 'POST', keepalive: true, credentials: 'same-origin', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ client: S.client }) });
    } catch (_) { /* best effort */ }
  });
  document.addEventListener('visibilitychange', () => {
    if (document.hidden || !S.started) return;
    heartbeat().then(() => { if (S.playing && S.view && S.view.stream && S.view.stream.status.state === 'live') reattach(); });
  });
  window.addEventListener('resize', () => { if (S.guideOpen) renderGuide(true); });


  (async function boot() {
    try {
      const r = await api('/api/status', undefined, { allow401: true });
      if (!r.data.configured) showPage('setup');
      else if (!r.data.authed) showPage('login');
      else startTV(false);
    } catch (_) {
      $('boot').innerHTML = '<p style="color:#F4F1EA;font:15px Geist,system-ui">Can’t reach the server. Retrying…</p>';
      setTimeout(() => location.reload(), 4000);
    }
  })();
})();
