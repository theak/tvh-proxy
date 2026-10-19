// Pure helpers shared by app.js and the node tests. No DOM access in here.
(function (root) {
  'use strict';

  const pad = (n) => String(n).padStart(2, '0');

  /** 24-hour HH:MM in local time, like the prototype. `ts` is unix seconds. */
  function fmtClock(ts) {
    const d = new Date(ts * 1000);
    return pad(d.getHours()) + ':' + pad(d.getMinutes());
  }

  /** "3h 15m", "45m", "2h". */
  function fmtDur(seconds) {
    const m = Math.max(0, Math.round(seconds / 60));
    const h = Math.floor(m / 60);
    const r = m % 60;
    if (h && r) return h + 'h ' + r + 'm';
    if (h) return h + 'h';
    return r + 'm';
  }

  /** "22 min left", "1h 5m left", "ending". */
  function fmtLeft(ev, now) {
    const left = ev.stop - now;
    if (left <= 60) return 'ending';
    if (left < 3600) return Math.round(left / 60) + ' min left';
    return fmtDur(left) + ' left';
  }

  /** "Sunday, Oct 18". */
  function fmtDate(ts) {
    return new Date(ts * 1000).toLocaleDateString(undefined, { weekday: 'long', month: 'short', day: 'numeric' });
  }

  /** Events grouped by channel uuid, each list sorted by start. */
  function indexEvents(events) {
    const by = new Map();
    for (const e of events) {
      if (!by.has(e.channel)) by.set(e.channel, []);
      by.get(e.channel).push(e);
    }
    for (const list of by.values()) list.sort((a, b) => a.start - b.start);
    return by;
  }

  /** { cur, next } for one channel at `now` (either may be null). */
  function nowNext(byChannel, channel, now) {
    const list = byChannel.get(channel) || [];
    let cur = null;
    let next = null;
    for (const e of list) {
      if (e.start <= now && now < e.stop) cur = e;
      else if (e.start >= now) { next = e; break; }
    }
    return { cur, next };
  }

  /** Fraction (0–1) of `ev` that has aired. */
  function progress(ev, now) {
    if (!ev) return 0;
    return Math.min(1, Math.max(0, (now - ev.start) / (ev.stop - ev.start)));
  }

  /** Wrap-around neighbour index in a channel list. `dir` is +1 (up) or -1 (down). */
  function step(len, idx, dir) {
    if (!len) return -1;
    return ((idx + dir) % len + len) % len;
  }

  /** Guide time window: starts at the half hour before `now`, `hours` long. */
  function guideWindow(now, hours) {
    const start = Math.floor(now / 1800) * 1800;
    return { start, end: start + hours * 3600 };
  }

  /**
   * Lay out one channel's programmes on the guide track. Returns cells with pixel
   * `left`/`width`, clipped to the window; gaps (no EPG) become nothing.
   */
  function layoutRow(list, win, pxPerMin) {
    const cells = [];
    for (const e of list || []) {
      if (e.stop <= win.start || e.start >= win.end) continue;
      const s = Math.max(e.start, win.start);
      const t = Math.min(e.stop, win.end);
      const left = ((s - win.start) / 60) * pxPerMin;
      const width = ((t - s) / 60) * pxPerMin;
      if (width < 4) continue;
      cells.push({ ev: e, left: Math.round(left) + 2, width: Math.max(2, Math.round(width) - 4) });
    }
    return cells;
  }

  /** Half-hour ruler ticks across the window. */
  function ticks(win, pxPerMin) {
    const out = [];
    for (let t = win.start; t < win.end; t += 1800) {
      out.push({ t, left: Math.round(((t - win.start) / 60) * pxPerMin), label: fmtClock(t) });
    }
    return out;
  }

  /** Index of the programme in `cells` that is airing at `now`, else the first upcoming, else 0. */
  function focusIndexFor(cells, now) {
    const airing = cells.findIndex((c) => c.ev.start <= now && now < c.ev.stop);
    if (airing >= 0) return airing;
    const upcoming = cells.findIndex((c) => c.ev.start > now);
    return upcoming >= 0 ? upcoming : 0;
  }

  /** In a row, the cell overlapping time `t` (or the nearest one). */
  function cellAtTime(cells, t) {
    if (!cells.length) return -1;
    const hit = cells.findIndex((c) => c.ev.start <= t && t < c.ev.stop);
    if (hit >= 0) return hit;
    let best = 0;
    let bestD = Infinity;
    cells.forEach((c, i) => {
      const d = Math.min(Math.abs(c.ev.start - t), Math.abs(c.ev.stop - t));
      if (d < bestD) { bestD = d; best = i; }
    });
    return best;
  }

  /** Random per-tab id (kept in sessionStorage so a reload keeps your role). */
  function newClientId() {
    const bytes = new Uint8Array(12);
    (root.crypto || require('crypto').webcrypto).getRandomValues(bytes);
    return Array.from(bytes, (b) => b.toString(16).padStart(2, '0')).join('');
  }

  const api = { fmtClock, fmtDur, fmtLeft, fmtDate, indexEvents, nowNext, progress, step, guideWindow, layoutRow, ticks, focusIndexFor, cellAtTime, newClientId };
  if (typeof module !== 'undefined' && module.exports) module.exports = api;
  else root.TVLib = api;
})(typeof window !== 'undefined' ? window : globalThis);
