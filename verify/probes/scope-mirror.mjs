/**
 * The live scope taps' path through the page: the feed frame's `scope` batch (`decodeFeedFrame`), the
 * store's plain mirror (`applyScope`) and the non-reactive getter a look reads it through
 * (`looper.scopeInto`, `src/ui/state/engine-store.ts`). Engine mode on the web engine fake
 * (`src/platform/host.web.ts`), booted as `engine-seam.mjs` boots it; the probe scripts batches through
 * `__lf.native.emit`, which decodes each one with the real decoder, and reads the mirror back.
 *
 * Proves: a batch's columns land oldest-first in the ring and `count`/`at` advance by its column count;
 * `frame` and `bin` describe the NEWEST column; a batch that does not splice (`gap`) drops the trace and
 * bumps `epoch`; a reset frame drops it too; the ring wraps at its length and `count` stops there, the
 * oldest columns falling off the back; and the decoder refuses a batch whose arrays disagree, leaving the
 * mirror untouched. No look draws any of this yet — this is the data path, not a picture.
 *
 * Cannot see the native engine (what the fake hands over is scripted, not folded), the 60 fps draw that
 * will read `scopeInto`, WebView2 or the eye.
 * Run: pnpm probe scope-mirror
 */
import assert from 'node:assert/strict';
import { probe } from '../harness/probe.ts';

const RATE = 48000;
/** `lf_engine::scope::scope_bin_frames(48_000)`: a 4 ms column. */
const BIN = 192;
/** `lf_engine::scope::SCOPE_SOURCES`: five lanes, the monitor, the master. */
const SOURCES = 7;
/** `SCOPE_COLUMNS` in `src/ui/state/engine-store.ts`. */
const RING = 1024;

const lane = () => ({
  state: 'Empty',
  length: 0,
  armed: false,
  autoArmed: false,
  canUndo: false,
  canReverse: false,
  reversed: false,
  stopAt: null,
  fading: false,
  retakePass: 0,
});

/** A batch of `values.length` columns from `frame`: source `s` carries `value + s / 100`, so a column
 * read back names both its place and its source. */
const batch = (frame, values, gap = false) => ({
  frame,
  bin: BIN,
  gap,
  min: Array.from({ length: SOURCES }, (_, s) => values.map((v) => -round(v + s / 100))),
  max: Array.from({ length: SOURCES }, (_, s) => values.map((v) => round(v + s / 100))),
});
/** Three decimals, as the feed rounds a column (`engine_io/feed.rs`). */
const round = (v) => Math.round(v * 1000) / 1000;

await probe(async ({ open }) => {
  const { page, consoleErrors } = await open({
    init: (p) => p.addInitScript(() => void (window.__lfEngineFake = true)),
  });
  let seq = 0;
  const emit = (frame) =>
    page.evaluate((f) => window.__lf.native.emit(f), { seq: ++seq, reset: false, events: [], device: [], anchor: null, meter: null, peaks: [], ...frame });

  await page.waitForFunction(() => window.__lf.native.opened.length === 1, undefined, { timeout: 5000 });
  await emit({
    reset: true,
    settings: [],
    events: [...[0, 1, 2, 3, 4].map((i) => ({ Lane: { frame: 0, lane: i, info: lane() } })), { Transport: { frame: 0, master: 0, bpm: 120, locked: false } }],
    anchor: { frame: 0, atMs: Date.now(), rate: RATE, grid: 0 },
    meter: { peak: 0, clip: false },
  });

  /** The mirror as a look would read it: the ring's stats, and the valid columns of `source` oldest
   * first (through the caller-owned view object `scopeInto` fills). */
  const mirror = (source = 0) =>
    page.evaluate(async (s) => {
      const { looper } = await import('/src/ui/state/audio.ts');
      const view = { lo: null, hi: null, at: 0, count: 0, frame: 0, bin: 0, epoch: 0 };
      const same = looper.scopeInto(view) === view;
      const order = (ring) => {
        const out = [];
        // The mirror holds f32s: six decimals is past their precision and keeps a 3-decimal column exact.
        for (let k = view.count; k > 0; k--) out.push(Math.round(ring[(view.at - k + ring.length) % ring.length] * 1e6) / 1e6);
        return out;
      };
      return {
        same,
        sources: view.lo.length,
        length: view.lo[0].length,
        at: view.at,
        count: view.count,
        frame: view.frame,
        bin: view.bin,
        epoch: view.epoch,
        lo: order(view.lo[s]),
        hi: order(view.hi[s]),
      };
    }, source);

  const start = await mirror();
  console.log(`mirror: ${start.sources} sources × ${start.length} columns, count ${start.count}, epoch ${start.epoch}`);
  assert.ok(start.same, 'scopeInto fills the caller-owned view and hands it back');
  assert.equal(start.sources, SOURCES, 'one ring per source');
  assert.equal(start.length, RING, 'the ring is preallocated whole');
  assert.equal(start.count, 0, 'nothing is valid before the first batch');

  // ── A first batch: it cannot splice onto anything, so the engine marks it `gap` ──────────────────
  await emit({ scope: batch(100_000, [0.1, 0.2, 0.3, 0.4], true) });
  const first = await mirror();
  console.log(`first batch: count ${first.count}, at ${first.at}, frame ${first.frame}, bin ${first.bin}, epoch ${first.epoch}`);
  assert.equal(first.count, 4);
  assert.equal(first.at, 4, 'the write cursor advanced by the batch');
  assert.equal(first.frame, 100_000 + 3 * BIN, 'frame names the newest column');
  assert.equal(first.bin, BIN);
  assert.ok(first.epoch > start.epoch, 'a gap bumps the epoch');
  assert.deepEqual(first.hi, [0.1, 0.2, 0.3, 0.4], 'oldest first, in the order the engine folded them');
  assert.deepEqual(first.lo, [-0.1, -0.2, -0.3, -0.4]);

  // ── A second batch that splices on: the trace grows, the epoch holds ─────────────────────────────
  await emit({ scope: batch(100_000 + 4 * BIN, [0.5, 0.6, 0.7]) });
  const second = await mirror();
  assert.equal(second.count, 7, 'the columns add to what was there');
  assert.equal(second.at, 7);
  assert.equal(second.frame, 100_000 + 6 * BIN);
  assert.equal(second.epoch, first.epoch, 'no gap, no epoch bump');
  assert.deepEqual(second.hi, [0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7], 'one trace, oldest first');
  const master = await mirror(SOURCES - 1);
  assert.deepEqual(master.hi, [0.16, 0.26, 0.36, 0.46, 0.56, 0.66, 0.76], 'every source its own ring (the master here)');

  // ── A batch the UI cannot splice: the trace it holds goes ────────────────────────────────────────
  await emit({ scope: batch(900_000, [0.9, 0.95], true) });
  const broke = await mirror();
  console.log(`after the gap: count ${broke.count}, at ${broke.at}, frame ${broke.frame}, epoch ${broke.epoch}`);
  assert.equal(broke.count, 2, 'the trace starts over at this batch');
  assert.equal(broke.at, 9, 'the cursor rides on: the ring is never rewound');
  assert.equal(broke.frame, 900_000 + BIN);
  assert.ok(broke.epoch > second.epoch, 'the epoch says the look must start over');
  assert.deepEqual(broke.hi, [0.9, 0.95]);

  // ── A reset frame (a new engine, a resync): the trace goes with it ───────────────────────────────
  await emit({ reset: true, settings: [], events: [], anchor: null, meter: null });
  const reset = await mirror();
  assert.equal(reset.count, 0, 'a reset frame drops the trace');
  assert.ok(reset.epoch > broke.epoch, 'and bumps the epoch');

  // ── The ring wraps: `count` stops at its length and the oldest columns fall off ──────────────────
  const total = RING + 2 * 64;
  let at = 1_000_000;
  for (let sent = 0; sent < total; sent += 64) {
    const values = Array.from({ length: 64 }, (_, k) => round((sent + k) / 10_000));
    await emit({ scope: batch(at, values, sent === 0) });
    at += 64 * BIN;
  }
  const wrapped = await mirror();
  console.log(`wrapped: ${total} columns sent, count ${wrapped.count}, at ${wrapped.at}, oldest ${wrapped.hi[0]}, newest ${wrapped.hi[wrapped.count - 1]}`);
  assert.equal(wrapped.count, RING, 'the ring holds its length and no more');
  assert.equal(wrapped.at, (reset.at + total) % RING, 'the cursor rode on from where it was and wrapped');
  assert.equal(wrapped.frame, at - BIN, 'frame still names the newest column');
  assert.equal(wrapped.hi[wrapped.count - 1], round((total - 1) / 10_000), 'the newest column is the last one sent');
  assert.equal(wrapped.hi[0], round((total - RING) / 10_000), 'the oldest valid column is the one RING columns back');

  // ── A batch the decoder refuses leaves the mirror as it was ─────────────────────────────────────
  const bad = batch(2_000_000, [0.1, 0.2]);
  bad.max[3].pop();
  await assert.rejects(() => emit({ scope: bad }), /scope/, 'arrays that disagree are refused');
  const after = await mirror();
  assert.deepEqual(
    [after.count, after.at, after.frame, after.epoch],
    [wrapped.count, wrapped.at, wrapped.frame, wrapped.epoch],
    'the refused batch changed nothing',
  );

  assert.deepEqual(consoleErrors, [], 'no console errors');
});
