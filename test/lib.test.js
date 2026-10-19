// Unit tests for static/js/lib.js — run with `npm test` (node's built-in runner).
const test = require('node:test');
const assert = require('node:assert/strict');
const L = require('../static/js/lib.js');

const ev = (channel, start, stop, title) => ({ channel, start, stop, title });

test('durations and time left read naturally', () => {
  assert.equal(L.fmtDur(3 * 3600 + 15 * 60), '3h 15m');
  assert.equal(L.fmtDur(45 * 60), '45m');
  assert.equal(L.fmtDur(2 * 3600), '2h');
  assert.equal(L.fmtLeft(ev('a', 0, 1000), 1000 - 22 * 60), '22 min left');
  assert.equal(L.fmtLeft(ev('a', 0, 10000), 10000 - 3900), '1h 5m left');
  assert.equal(L.fmtLeft(ev('a', 0, 100), 90), 'ending');
});

test('clock is zero-padded 24h local time', () => {
  const d = new Date(2026, 9, 18, 7, 5, 0);
  assert.equal(L.fmtClock(d.getTime() / 1000), '07:05');
});

test('nowNext finds the airing and following programme', () => {
  const by = L.indexEvents([ev('a', 200, 300, 'Later'), ev('a', 100, 200, 'Now'), ev('b', 0, 1000, 'Other')]);
  const nn = L.nowNext(by, 'a', 150);
  assert.equal(nn.cur.title, 'Now');
  assert.equal(nn.next.title, 'Later');
  assert.deepEqual(L.nowNext(by, 'missing', 150), { cur: null, next: null });
  assert.equal(L.nowNext(by, 'a', 50).cur, null);
  assert.equal(L.nowNext(by, 'a', 50).next.title, 'Now');
});

test('progress is clamped', () => {
  assert.equal(L.progress(ev('a', 100, 200), 150), 0.5);
  assert.equal(L.progress(ev('a', 100, 200), 50), 0);
  assert.equal(L.progress(ev('a', 100, 200), 500), 1);
  assert.equal(L.progress(null, 5), 0);
});

test('channel stepping wraps around', () => {
  assert.equal(L.step(3, 2, 1), 0);
  assert.equal(L.step(3, 0, -1), 2);
  assert.equal(L.step(0, 0, 1), -1);
});

test('guide window starts on the half hour', () => {
  const w = L.guideWindow(1800 * 10 + 700, 8);
  assert.equal(w.start, 1800 * 10);
  assert.equal(w.end, 1800 * 10 + 8 * 3600);
});

test('row layout clips to the window and drops slivers', () => {
  const win = { start: 1000, end: 1000 + 3600 };
  const cells = L.layoutRow([ev('a', 0, 1000 + 1800, 'Started earlier'), ev('a', 2800, 4600, 'Ends later'), ev('a', 9000, 9100, 'Outside')], win, 2);
  assert.equal(cells.length, 2);
  assert.equal(cells[0].left, 2); // clipped to window start (+2px gutter)
  assert.equal(cells[0].width, 30 * 2 - 4);
  assert.equal(cells[1].ev.title, 'Ends later');
  assert.equal(cells[1].left + cells[1].width, 60 * 2 - 2);
});

test('ticks every half hour', () => {
  const t = L.ticks({ start: 0, end: 7200 }, 4);
  assert.deepEqual(t.map((x) => x.left), [0, 120, 240, 360]);
});

test('focus picks the airing cell, else the next one', () => {
  const cells = [{ ev: ev('a', 0, 100) }, { ev: ev('a', 100, 200) }, { ev: ev('a', 200, 300) }];
  assert.equal(L.focusIndexFor(cells, 150), 1);
  assert.equal(L.focusIndexFor([{ ev: ev('a', 300, 400) }], 150), 0);
  assert.equal(L.cellAtTime(cells, 250), 2);
  assert.equal(L.cellAtTime(cells, 999), 2);
  assert.equal(L.cellAtTime([], 5), -1);
});

test('client ids are random hex', () => {
  const a = L.newClientId();
  assert.match(a, /^[0-9a-f]{24}$/);
  assert.notEqual(a, L.newClientId());
});
